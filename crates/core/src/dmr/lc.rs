//! Link control: who is calling whom (ETSI TS 102 361-2 section 7.1).

use super::fec;

/// Full link control opcodes this decoder understands.
pub const FLCO_GROUP: u8 = 0;
pub const FLCO_PRIVATE: u8 = 3;
pub const FLCO_ALIAS_HEADER: u8 = 4;
pub const FLCO_ALIAS_BLOCK3: u8 = 7;

/// CRC masks of the full LC, by burst type.
pub const MASK_VOICE_HEADER: [u8; 3] = [0x96; 3];
pub const MASK_TERMINATOR: [u8; 3] = [0x99; 3];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lc {
    pub flco: u8,
    /// Feature set id (0 standard, 16 DMRA, ...).
    pub fid: u8,
    /// Service options: bit 7 emergency, bit 6 privacy (encrypted).
    pub options: u8,
    pub dst: u32,
    pub src: u32,
}

impl Lc {
    pub fn group(dst: u32, src: u32) -> Self {
        Lc {
            flco: FLCO_GROUP,
            fid: 0,
            options: 0,
            dst,
            src,
        }
    }

    pub fn from_bytes(b: &[u8; 9]) -> Self {
        Lc {
            flco: b[0] & 0x3F,
            fid: b[1],
            options: b[2],
            dst: u32::from_be_bytes([0, b[3], b[4], b[5]]),
            src: u32::from_be_bytes([0, b[6], b[7], b[8]]),
        }
    }

    pub fn to_bytes(&self) -> [u8; 9] {
        let d = self.dst.to_be_bytes();
        let s = self.src.to_be_bytes();
        [
            self.flco & 0x3F,
            self.fid,
            self.options,
            d[1],
            d[2],
            d[3],
            s[1],
            s[2],
            s[3],
        ]
    }

    /// A voice call's link control (group or private), as opposed to talker
    /// alias, GPS and vendor opcodes that share the embedded channel.
    pub fn is_call(&self) -> bool {
        self.flco == FLCO_GROUP || self.flco == FLCO_PRIVATE
    }

    pub fn encrypted(&self) -> bool {
        self.options & 0x40 != 0
    }

    pub fn emergency(&self) -> bool {
        self.options & 0x80 != 0
    }
}

/// Full LC from the 96 decoded BPTC bits of a voice header or terminator,
/// if its Reed-Solomon check (under the burst type's mask) holds.
pub fn full_lc(bits: &[u8; 96], mask: [u8; 3]) -> Option<Lc> {
    let b = fec::bits_to_bytes(bits);
    let lc: [u8; 9] = b[..9].try_into().unwrap();
    let p = fec::rs129_parity(&lc);
    let ok = (0..3).all(|i| b[9 + i] ^ mask[i] == p[i]);
    ok.then(|| Lc::from_bytes(&lc))
}

/// The 96 bits to BPTC-encode for a voice header or terminator.
pub fn full_lc_bits(lc: &Lc, mask: [u8; 3]) -> [u8; 96] {
    let b = lc.to_bytes();
    let p = fec::rs129_parity(&b);
    let mut all = b.to_vec();
    all.extend((0..3).map(|i| p[i] ^ mask[i]));
    fec::bytes_to_bits(&all).try_into().unwrap()
}

/// Collects the 4 blocks of a talker alias (embedded LC opcodes 4..7, 7
/// bytes each) and decodes the text.
#[derive(Debug, Clone, Default)]
pub struct TalkerAlias {
    buf: [u8; 28],
    have: u8,
}

impl TalkerAlias {
    /// Add a block from an embedded LC with opcode 4..=7.
    pub fn add(&mut self, lc: &[u8; 9]) {
        let block = (lc[0] & 0x3F).wrapping_sub(FLCO_ALIAS_HEADER) as usize;
        if block > 3 {
            return;
        }
        self.buf[block * 7..block * 7 + 7].copy_from_slice(&lc[2..9]);
        self.have |= 1 << block;
    }

    /// The alias so far, once the header and the blocks it needs are in.
    pub fn text(&self) -> Option<String> {
        if self.have & 1 == 0 {
            return None;
        }
        let format = self.buf[0] >> 6;
        let len = ((self.buf[0] >> 1) & 0x1F) as usize;
        let chars: Vec<char> = match format {
            0 => {
                // 7-bit characters packed from the header's last bit.
                let bits = fec::bytes_to_bits(&self.buf);
                bits[7..]
                    .chunks_exact(7)
                    .take(len)
                    .map(|c| fec::to_u32(c) as u8 as char)
                    .collect()
            }
            1 | 2 => {
                let bytes: Vec<u8> = self.buf[1..].iter().take(len).copied().collect();
                if format == 2 {
                    String::from_utf8_lossy(&bytes).chars().collect()
                } else {
                    bytes.iter().map(|&b| b as char).collect()
                }
            }
            _ => self.buf[1..]
                .chunks_exact(2)
                .take(len)
                .map(|p| char::from_u32(u16::from_be_bytes([p[0], p[1]]) as u32).unwrap_or('?'))
                .collect(),
        };
        // Complete only once every character has arrived (blocks the alias
        // needs are all in, and none decoded as NUL).
        let needed = match format {
            0 => (7 + 7 * len).div_ceil(56),
            3 => (1 + 2 * len).div_ceil(7),
            _ => (1 + len).div_ceil(7),
        }
        .clamp(1, 4);
        let all = (0..needed).all(|b| self.have & (1 << b) != 0);
        let text: String = chars.into_iter().filter(|c| *c != '\0').collect();
        (all && text.chars().count() == len && len > 0).then(|| text.trim().to_string())
    }

    /// Encode `text` (ASCII, up to 31 characters) as 7-bit alias blocks:
    /// the embedded LCs with opcodes 4..=7 that carry it (for tests).
    pub fn encode(text: &str) -> Vec<[u8; 9]> {
        let chars: Vec<u8> = text.bytes().filter(|c| c.is_ascii()).take(31).collect();
        let mut bits = vec![0u8; 28 * 8];
        fec::from_u32(chars.len() as u32, 5, &mut bits[2..]);
        for (i, &c) in chars.iter().enumerate() {
            fec::from_u32(c as u32, 7, &mut bits[7 + 7 * i..]);
        }
        let bytes = fec::bits_to_bytes(&bits);
        let blocks = (7 + 7 * chars.len()).div_ceil(56);
        (0..blocks)
            .map(|b| {
                let mut lc = [0u8; 9];
                lc[0] = FLCO_ALIAS_HEADER + b as u8;
                lc[2..9].copy_from_slice(&bytes[b * 7..b * 7 + 7]);
                lc
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_lc_round_trip_and_masks() {
        let lc = Lc::group(99, 3_171_501);
        let bits = full_lc_bits(&lc, MASK_VOICE_HEADER);
        assert_eq!(full_lc(&bits, MASK_VOICE_HEADER), Some(lc));
        // The terminator's mask doesn't check out on a header.
        assert_eq!(full_lc(&bits, MASK_TERMINATOR), None);
    }

    #[test]
    fn talker_alias_seven_bit() {
        let mut ta = TalkerAlias::default();
        let blocks = TalkerAlias::encode("KF7HVM Masen");
        assert_eq!(blocks.len(), 2);
        ta.add(&blocks[0]);
        assert_eq!(ta.text(), None);
        ta.add(&blocks[1]);
        assert_eq!(ta.text().as_deref(), Some("KF7HVM Masen"));
    }
}
