//! The error-correcting codes DMR uses (ETSI TS 102 361-1 annex B).
//!
//! Bits are `u8` values 0 or 1, most significant first, as they sit in a
//! burst. Small codes decode to the nearest codeword; the block product code
//! iterates its row and column Hamming codes.

use std::sync::OnceLock;

/// Bits to an integer, most significant first.
pub fn to_u32(bits: &[u8]) -> u32 {
    bits.iter().fold(0, |a, &b| (a << 1) | (b & 1) as u32)
}

/// The low `n` bits of `v` as bits, most significant first.
pub fn from_u32(v: u32, n: usize, out: &mut [u8]) {
    for (i, b) in out[..n].iter_mut().enumerate() {
        *b = ((v >> (n - 1 - i)) & 1) as u8;
    }
}

pub fn bits_to_bytes(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8).map(|c| to_u32(c) as u8).collect()
}

pub fn bytes_to_bits(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; bytes.len() * 8];
    for (i, &b) in bytes.iter().enumerate() {
        from_u32(b as u32, 8, &mut out[i * 8..]);
    }
    out
}

/// Remainder of `v` (degree < `top`) divided by the generator `g` of degree
/// `deg`.
fn poly_rem(mut v: u32, g: u32, deg: u32, top: u32) -> u32 {
    for i in (deg..top).rev() {
        if v & (1 << i) != 0 {
            v ^= g << (i - deg);
        }
    }
    v
}

/// The Golay generator x^11+x^10+x^6+x^5+x^4+x^2+1.
const GOLAY_G: u32 = 0xC75;

/// Golay (23,12): 12 data bits, then 11 parity bits.
pub fn golay23_encode(data: u32) -> u32 {
    let d = data & 0xFFF;
    (d << 11) | poly_rem(d << 11, GOLAY_G, 11, 23)
}

/// Corrects up to three errors (the code is perfect, so every word decodes).
/// Returns the 12 data bits and the number of bits corrected.
pub fn golay23_decode(word: u32) -> (u32, u32) {
    static TABLE: OnceLock<Vec<u32>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = vec![0u32; 2048];
        let mut fill = |e: u32| {
            let s = poly_rem(e, GOLAY_G, 11, 23) as usize;
            if t[s] == 0 {
                t[s] = e;
            }
        };
        for a in 0..23 {
            fill(1 << a);
            for b in a + 1..23 {
                fill((1 << a) | (1 << b));
                for c in b + 1..23 {
                    fill((1 << a) | (1 << b) | (1 << c));
                }
            }
        }
        t
    });
    let w = word & 0x7F_FFFF;
    let e = table[poly_rem(w, GOLAY_G, 11, 23) as usize];
    ((w ^ e) >> 11, e.count_ones())
}

/// Extended Golay (24,12): the (23,12) word then an even parity bit.
pub fn golay24_encode(data: u32) -> u32 {
    let w = golay23_encode(data);
    (w << 1) | (w.count_ones() & 1)
}

/// Returns the data, the bits corrected, and whether the word was beyond
/// correction (four errors show up as a parity mismatch after correcting
/// three).
pub fn golay24_decode(word: u32) -> (u32, u32, bool) {
    let (d, n) = golay23_decode(word >> 1);
    let ok = golay24_encode(d) ^ (word & 0xFF_FFFF);
    let errs = ok.count_ones();
    (d, errs.max(n), errs > 3)
}

/// Nearest codeword among `words` (data is the index). Returns the data and
/// the distance.
fn nearest(words: &[u32], w: u32) -> (u32, u32) {
    let mut best = (0, u32::MAX);
    for (d, &c) in words.iter().enumerate() {
        let dist = (c ^ w).count_ones();
        if dist < best.1 {
            best = (d as u32, dist);
        }
    }
    best
}

/// Golay (20,8), the slot type code: 8 data bits, the 11 parity bits of the
/// shortened (23,12) code, then an even parity bit.
pub fn golay20_encode(data: u32) -> u32 {
    let d = data & 0xFF;
    let p = poly_rem(d << 11, GOLAY_G, 11, 19);
    let w = (d << 11) | p;
    (w << 1) | (w.count_ones() & 1)
}

/// The nearest codeword's data and the distance to it (up to 3 is
/// correctable).
pub fn golay20_decode(word: u32) -> (u32, u32) {
    static WORDS: OnceLock<Vec<u32>> = OnceLock::new();
    let words = WORDS.get_or_init(|| (0..256).map(golay20_encode).collect());
    nearest(words, word & 0xF_FFFF)
}

/// Quadratic residue (16,7), the EMB code: the cyclic (15,7) code with
/// generator x^8+x^5+x^4+x^3+1, then an even parity bit.
pub fn qr16_encode(data: u32) -> u32 {
    let d = data & 0x7F;
    let w = (d << 8) | poly_rem(d << 8, 0x139, 8, 15);
    (w << 1) | (w.count_ones() & 1)
}

/// The nearest codeword's data and the distance (up to 2 is correctable).
pub fn qr16_decode(word: u32) -> (u32, u32) {
    static WORDS: OnceLock<Vec<u32>> = OnceLock::new();
    let words = WORDS.get_or_init(|| (0..128).map(qr16_encode).collect());
    nearest(words, word & 0xFFFF)
}

/// A systematic Hamming code given by its parity equations: parity bit `j`
/// (at position `k + j`) is the XOR of the data bits listed in `eqs[j]`.
struct Hamming {
    k: usize,
    eqs: &'static [&'static [usize]],
}

impl Hamming {
    fn encode(&self, d: &mut [u8]) {
        for (j, eq) in self.eqs.iter().enumerate() {
            d[self.k + j] = eq.iter().fold(0, |a, &i| a ^ d[i]);
        }
    }

    /// Corrects a single error in place. Returns false if the word has
    /// errors it can't place.
    fn decode(&self, d: &mut [u8]) -> bool {
        let syn = |d: &[u8]| -> u32 {
            self.eqs.iter().enumerate().fold(0, |s, (j, eq)| {
                let p = eq.iter().fold(d[self.k + j], |a, &i| a ^ d[i]);
                s | ((p as u32) << j)
            })
        };
        let s = syn(d);
        if s == 0 {
            return true;
        }
        let n = self.k + self.eqs.len();
        let found = (0..n).find(|&pos| {
            let col = if pos < self.k {
                self.eqs
                    .iter()
                    .enumerate()
                    .fold(0, |c, (j, eq)| c | ((eq.contains(&pos) as u32) << j))
            } else {
                1 << (pos - self.k)
            };
            col == s
        });
        if let Some(pos) = found {
            d[pos] ^= 1;
        }
        found.is_some()
    }
}

/// Hamming (15,11,3): the rows of the block product code.
const H15_11: Hamming = Hamming {
    k: 11,
    eqs: &[
        &[0, 1, 2, 3, 5, 7, 8],
        &[1, 2, 3, 4, 6, 8, 9],
        &[2, 3, 4, 5, 7, 9, 10],
        &[0, 1, 2, 4, 6, 7, 10],
    ],
};

/// Hamming (13,9,3): the columns of the block product code.
const H13_9: Hamming = Hamming {
    k: 9,
    eqs: &[
        &[0, 1, 3, 5, 6],
        &[0, 1, 2, 4, 6, 7],
        &[0, 1, 2, 3, 5, 7, 8],
        &[0, 2, 4, 5, 8],
    ],
};

/// Hamming (16,11,4): the rows of the embedded signalling.
const H16_11: Hamming = Hamming {
    k: 11,
    eqs: &[
        &[0, 1, 2, 3, 5, 7, 8],
        &[1, 2, 3, 4, 6, 8, 9],
        &[2, 3, 4, 5, 7, 9, 10],
        &[0, 1, 2, 4, 6, 7, 10],
        &[0, 2, 5, 6, 8, 9, 10],
    ],
};

/// Where data bits sit in the deinterleaved BPTC (196,96) matrix of 13 rows
/// by 15 columns, after the unused first bit.
fn bptc_data_positions() -> impl Iterator<Item = usize> {
    (4..=11).chain((1..9).flat_map(|r| (r * 15 + 1)..=(r * 15 + 11)))
}

/// Block product turbo code (196,96): 96 bits (12 bytes) in, 196 bits out.
pub fn bptc_encode(data: &[u8; 96]) -> [u8; 196] {
    let mut m = [0u8; 196];
    for (p, &b) in bptc_data_positions().zip(data.iter()) {
        m[p] = b;
    }
    for r in 0..9 {
        H15_11.encode(&mut m[r * 15 + 1..r * 15 + 16]);
    }
    for c in 0..15 {
        let mut col = [0u8; 13];
        for (a, v) in col.iter_mut().enumerate() {
            *v = m[c + 1 + a * 15];
        }
        H13_9.encode(&mut col);
        for (a, v) in col.iter().enumerate() {
            m[c + 1 + a * 15] = *v;
        }
    }
    let mut out = [0u8; 196];
    for (a, &v) in m.iter().enumerate() {
        out[(a * 181) % 196] = v;
    }
    out
}

/// Decodes 196 received bits; returns the 96 data bits and whether every
/// row and column checked out after correction.
pub fn bptc_decode(raw: &[u8]) -> ([u8; 96], bool) {
    let mut m = [0u8; 196];
    for (a, v) in m.iter_mut().enumerate() {
        *v = raw[(a * 181) % 196];
    }
    let mut clean = false;
    for _ in 0..5 {
        clean = true;
        let mut fixed = false;
        for c in 0..15 {
            let mut col = [0u8; 13];
            for (a, v) in col.iter_mut().enumerate() {
                *v = m[c + 1 + a * 15];
            }
            let before = col;
            if !H13_9.decode(&mut col) {
                clean = false;
            }
            if col != before {
                fixed = true;
                for (a, v) in col.iter().enumerate() {
                    m[c + 1 + a * 15] = *v;
                }
            }
        }
        for r in 0..9 {
            let row = &mut m[r * 15 + 1..r * 15 + 16];
            let before: [u8; 15] = row.try_into().unwrap();
            if !H15_11.decode(row) {
                clean = false;
            }
            if row != before {
                fixed = true;
            }
        }
        if !fixed {
            break;
        }
    }
    let mut out = [0u8; 96];
    for (o, p) in out.iter_mut().zip(bptc_data_positions()) {
        *o = m[p];
    }
    (out, clean)
}

/// GF(256) with the polynomial x^8+x^4+x^3+x^2+1, for Reed-Solomon (12,9).
fn gf_mul(a: u8, b: u8) -> u8 {
    let (mut a, mut b, mut p) = (a as u16, b, 0u16);
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        a <<= 1;
        if a & 0x100 != 0 {
            a ^= 0x11D;
        }
        b >>= 1;
    }
    p as u8
}

/// The three Reed-Solomon (12,9) parity bytes for 9 bytes of link control,
/// in the order they are sent.
pub fn rs129_parity(msg: &[u8]) -> [u8; 3] {
    // Generator (x+a)(x+a^2)(x+a^3) = x^3 + 14x^2 + 56x + 64.
    const G: [u8; 3] = [64, 56, 14];
    let mut p = [0u8; 3];
    for &m in &msg[..9] {
        let fb = m ^ p[2];
        p[2] = p[1] ^ gf_mul(G[2], fb);
        p[1] = p[0] ^ gf_mul(G[1], fb);
        p[0] = gf_mul(G[0], fb);
    }
    [p[2], p[1], p[0]]
}

/// The 5-bit checksum of embedded link control: the sum of its 9 bytes
/// modulo 31.
pub fn crc5(lc: &[u8]) -> u32 {
    lc[..9].iter().map(|&b| b as u32).sum::<u32>() % 31
}

/// Embedded signalling: 72 bits of link control plus its checksum, as 128
/// bits sent 32 at a time in voice bursts B to E.
pub fn embedded_encode(lc: &[u8; 9]) -> [u8; 128] {
    let bits = bytes_to_bits(lc);
    let crc = crc5(lc);
    let mut d = [0u8; 128];
    let mut b = 0;
    for (start, len) in EMBEDDED_ROWS {
        d[start..start + len].copy_from_slice(&bits[b..b + len]);
        b += len;
    }
    for (i, pos) in [42, 58, 74, 90, 106].into_iter().enumerate() {
        d[pos] = ((crc >> (4 - i)) & 1) as u8;
    }
    for r in 0..7 {
        H16_11.encode(&mut d[r * 16..r * 16 + 16]);
    }
    for a in 0..16 {
        d[112 + a] = (0..7).fold(0, |p, r| p ^ d[r * 16 + a]);
    }
    let mut raw = [0u8; 128];
    let mut b = 0;
    for v in raw.iter_mut() {
        *v = d[b];
        b += 16;
        if b > 127 {
            b -= 127;
        }
    }
    raw
}

/// The 128 received bits back to 9 bytes of link control, if the Hamming
/// rows, column parity and checksum all agree.
pub fn embedded_decode(raw: &[u8; 128]) -> Option<[u8; 9]> {
    let mut d = [0u8; 128];
    let mut b = 0;
    for &v in raw.iter() {
        d[b] = v;
        b += 16;
        if b > 127 {
            b -= 127;
        }
    }
    for r in 0..7 {
        if !H16_11.decode(&mut d[r * 16..r * 16 + 16]) {
            return None;
        }
    }
    if (0..16).any(|a| (0..8).fold(0, |p, r| p ^ d[r * 16 + a]) != 0) {
        return None;
    }
    let mut bits = Vec::with_capacity(72);
    for (start, len) in EMBEDDED_ROWS {
        bits.extend_from_slice(&d[start..start + len]);
    }
    let lc: [u8; 9] = bits_to_bytes(&bits).try_into().unwrap();
    let crc = [42, 58, 74, 90, 106]
        .into_iter()
        .fold(0, |c, p| (c << 1) | d[p] as u32);
    (crc5(&lc) == crc).then_some(lc)
}

/// (start, length) of the link control bits in each row of the embedded
/// signalling matrix.
const EMBEDDED_ROWS: [(usize, usize); 7] = [
    (0, 11),
    (16, 11),
    (32, 10),
    (48, 10),
    (64, 10),
    (80, 10),
    (96, 10),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golay20_matches_the_slot_type_table() {
        // First entries of the published Golay (20,8) encoding table, as the
        // 12 parity bits after the data byte.
        assert_eq!(golay20_encode(1) & 0xFFF, 0x8EB);
        assert_eq!(golay20_encode(2) & 0xFFF, 0x93E);
        for d in 0..256 {
            let w = golay20_encode(d);
            for e in [0u32, 1 << 3, (1 << 19) | 1, (1 << 7) | (1 << 12) | 1] {
                assert_eq!(golay20_decode(w ^ e).0, d);
            }
        }
    }

    #[test]
    fn qr16_matches_the_emb_table() {
        assert_eq!(qr16_encode(1), 0x0273);
        assert_eq!(qr16_encode(2), 0x04E5);
        assert_eq!(qr16_encode(0x7F), 0xFE5B);
        for d in 0..128 {
            let w = qr16_encode(d);
            assert_eq!(qr16_decode(w ^ 0x8001).0, d);
        }
    }

    #[test]
    fn golay24_corrects_three() {
        for d in [0u32, 1, 0x5A5, 0xFFF] {
            let w = golay24_encode(d);
            assert_eq!(golay24_decode(w), (d, 0, false));
            let (got, n, bad) = golay24_decode(w ^ 0b1000_0100_0000_0000_0010_0000);
            assert_eq!((got, n, bad), (d, 3, false));
        }
    }

    #[test]
    fn bptc_round_trip_with_errors() {
        let mut data = [0u8; 96];
        for (i, b) in data.iter_mut().enumerate() {
            *b = ((i * 7 + 3) % 5 == 0) as u8;
        }
        let mut code = bptc_encode(&data);
        assert_eq!(bptc_decode(&code), (data, true));
        for p in [5, 50, 100, 150, 190] {
            code[p] ^= 1;
        }
        assert_eq!(bptc_decode(&code).0, data);
    }

    #[test]
    fn rs129_known_lc() {
        // Group call to TG 9 from 1234567: parity checks against itself and
        // catches a changed byte.
        let lc = [0u8, 0, 0, 0, 0, 9, 0x12, 0xD6, 0x87];
        let p = rs129_parity(&lc);
        let mut bad = lc;
        bad[5] = 8;
        assert_ne!(rs129_parity(&bad), p);
    }

    #[test]
    fn embedded_round_trip() {
        let lc = [0u8, 0, 0, 0, 0, 99, 0x30, 0x64, 0xAD];
        let mut raw = embedded_encode(&lc);
        assert_eq!(embedded_decode(&raw), Some(lc));
        // One error in each of several rows is corrected.
        for p in [40, 41, 42] {
            raw[p] ^= 1;
        }
        assert_eq!(embedded_decode(&raw), Some(lc));
        // Two errors in one row are not. Raw bit a holds matrix bit 16a
        // mod 127, so raw bits 8 apart share a row.
        let mut raw = embedded_encode(&lc);
        raw[1] ^= 1;
        raw[9] ^= 1;
        assert_eq!(embedded_decode(&raw), None);
    }
}
