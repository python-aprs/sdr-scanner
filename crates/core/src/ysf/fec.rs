//! System Fusion's channel coding: the rate 1/2, K=5 convolutional code
//! that protects FICH and the data channel, their CRC, the whitening
//! sequence and the interleavers. Bits are `u8` 0 or 1, most significant
//! first, as in [`crate::dmr::fec`].

/// Whitening: XORed over the data channel's bytes before its CRC, and over
/// a V/D mode 2 voice channel's 104 bits.
pub const WHITENING: [u8; 20] = [
    0x93, 0xD7, 0x51, 0x21, 0x9C, 0x2F, 0x6C, 0xD0, 0xEF, 0x0F, 0xF8, 0x3D, 0xF1, 0x73, 0x20, 0x94,
    0xED, 0x1E, 0x7C, 0xD8,
];

/// Bit `i` of the whitening sequence.
pub fn whitening_bit(i: usize) -> u8 {
    (WHITENING[i / 8] >> (7 - i % 8)) & 1
}

/// The two code bits for input `d` with the encoder holding `state` (the
/// last four inputs, newest in bit 0): G1 = 1 + D^3 + D^4,
/// G2 = 1 + D + D^2 + D^4.
fn branch(state: u8, d: u8) -> (u8, u8) {
    let (d1, d2, d3, d4) = (
        state & 1,
        (state >> 1) & 1,
        (state >> 2) & 1,
        (state >> 3) & 1,
    );
    (d ^ d3 ^ d4, d ^ d1 ^ d2 ^ d4)
}

/// Convolutionally encodes `bits` (the caller appends the four zero tail
/// bits), two code bits per input bit.
pub fn conv_encode(bits: &[u8]) -> Vec<u8> {
    let mut state = 0u8;
    let mut out = Vec::with_capacity(bits.len() * 2);
    for &d in bits {
        let (g1, g2) = branch(state, d & 1);
        out.push(g1);
        out.push(g2);
        state = ((state << 1) | (d & 1)) & 0xF;
    }
    out
}

/// Hard-decision Viterbi decoder for [`conv_encode`]'s code, for a block
/// that starts and (after its four tail bits) ends in the zero state.
/// Returns the `code.len() / 2 - 4` data bits and the number of code bits
/// that disagree with the decoded path.
pub fn viterbi(code: &[u8]) -> (Vec<u8>, u32) {
    const INF: u32 = u32::MAX / 2;
    let n = code.len() / 2;
    let mut metric = [INF; 16];
    metric[0] = 0;
    // For each step and state: the previous state.
    let mut from = vec![[0u8; 16]; n];
    for (t, pair) in code.chunks_exact(2).enumerate() {
        let mut next = [INF; 16];
        for (s, &m) in metric.iter().enumerate() {
            if m >= INF {
                continue;
            }
            for d in 0..2u8 {
                let (g1, g2) = branch(s as u8, d);
                let cost = m + (g1 != pair[0]) as u32 + (g2 != pair[1]) as u32;
                let ns = (((s as u8) << 1) | d) as usize & 0xF;
                if cost < next[ns] {
                    next[ns] = cost;
                    from[t][ns] = s as u8;
                }
            }
        }
        metric = next;
    }
    let mut bits = vec![0u8; n];
    let mut s = 0u8;
    for t in (0..n).rev() {
        bits[t] = s & 1;
        s = from[t][s as usize];
    }
    bits.truncate(n.saturating_sub(4));
    (bits, metric[0])
}

/// CRC-16 over `bytes` as System Fusion uses it: CCITT polynomial, zero
/// initial value, inverted, sent high byte first.
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in bytes {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    !crc
}

/// Whether the last two bytes of `bytes` are the CRC of the rest.
pub fn crc_ok(bytes: &[u8]) -> bool {
    let n = bytes.len();
    n > 2 && crc16(&bytes[..n - 2]).to_be_bytes() == bytes[n - 2..]
}

/// Position of code bit pair `i` in a block interleaved `cols` dibits by 20
/// (FICH and V/D mode 2 data: 5 columns; headers and V/D mode 1 data: 9).
pub fn dibit_interleave(i: usize, cols: usize) -> usize {
    (i % cols) * 40 + (i / cols) * 2
}

/// Position of bit `i` of a V/D mode 2 voice channel (26 by 4, by bit).
pub fn vch_interleave(i: usize) -> usize {
    (i % 26) * 4 + i / 26
}

/// Undoes the dibit interleave of a `cols` * 40 bit block and decodes its
/// convolutional code. Returns the `cols` * 20 - 4 data bits and the code
/// bits corrected.
pub fn deconv(raw: &[u8], cols: usize) -> (Vec<u8>, u32) {
    let pairs = cols * 20;
    let mut code = vec![0u8; pairs * 2];
    for i in 0..pairs {
        let n = dibit_interleave(i, cols);
        code[2 * i] = raw[n];
        code[2 * i + 1] = raw[n + 1];
    }
    viterbi(&code)
}

/// Encodes `cols` * 20 - 4 data bits into an interleaved block of
/// `cols` * 40 bits: the inverse of [`deconv`].
pub fn conv(bits: &[u8], cols: usize) -> Vec<u8> {
    let mut bits = bits.to_vec();
    bits.extend([0u8; 4]);
    let code = conv_encode(&bits);
    let mut raw = vec![0u8; cols * 40];
    for i in 0..cols * 20 {
        let n = dibit_interleave(i, cols);
        raw[n] = code[2 * i];
        raw[n + 1] = code[2 * i + 1];
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viterbi_corrects_scattered_errors() {
        let data: Vec<u8> = (0..96).map(|i| ((i * 7 + i / 3) % 5 == 0) as u8).collect();
        let mut bits = data.clone();
        bits.extend([0; 4]);
        let mut code = conv_encode(&bits);
        for i in [3, 40, 91, 150, 197] {
            code[i] ^= 1;
        }
        let (got, errs) = viterbi(&code);
        assert_eq!(got, data);
        assert_eq!(errs, 5);
    }

    #[test]
    fn crc_matches_known_value() {
        // CRC-16/CCITT with zero init over "123456789" is 0x31C3; System
        // Fusion sends it inverted.
        assert_eq!(crc16(b"123456789"), !0x31C3);
        let mut b = b"KF7HVM".to_vec();
        b.extend(crc16(&b).to_be_bytes());
        assert!(crc_ok(&b));
        b[2] ^= 4;
        assert!(!crc_ok(&b));
    }

    #[test]
    fn interleavers_are_permutations() {
        for cols in [5, 9] {
            let mut seen = vec![false; cols * 40];
            for i in 0..cols * 20 {
                let n = dibit_interleave(i, cols);
                assert!(!seen[n] && !seen[n + 1]);
                seen[n] = true;
                seen[n + 1] = true;
            }
        }
        let mut seen = [false; 104];
        for i in 0..104 {
            seen[vch_interleave(i)] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }
}
