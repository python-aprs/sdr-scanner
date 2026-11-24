//! What sits where in a 264-bit DMR burst, and the 24-bit CACH before it on
//! a repeater's output (ETSI TS 102 361-1 section 9).
//!
//! ```text
//! | 108 bits payload | 48 bits sync or EMB+embedded | 108 bits payload |
//! data bursts:  98 info | 10 slot type | sync | 10 slot type | 98 info
//! voice bursts: 3 AMBE+2 frames of 72 bits across both payload halves
//! ```

use super::fec;

pub const BURST_BITS: usize = 264;
pub const BURST_SYMBOLS: usize = 132;
pub const CACH_SYMBOLS: usize = 12;
/// A TDMA frame: CACH plus one burst, 30 ms.
pub const FRAME_SYMBOLS: usize = 144;
pub const SYNC_SYMBOLS: usize = 24;
/// Symbol index of the sync within a burst.
pub const SYNC_START: usize = 54;
pub const SYMBOL_RATE: f64 = 4800.0;
/// Deviation of the outer symbols (+3/-3), Hz.
pub const OUTER_DEV: f64 = 1944.0;

/// Who sent a burst, as its sync pattern says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// A repeater's output (continuous, with CACH).
    Base,
    /// A radio talking to a repeater or another radio.
    Mobile,
    /// TDMA direct mode, timeslot 1 or 2.
    Direct(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sync {
    pub source: Source,
    /// Voice (burst A of a superframe) or data/control.
    pub voice: bool,
}

/// Voice sync patterns; each data pattern is the same with every symbol
/// negated (XOR 0xAAAAAAAAAAAA).
pub const VOICE_SYNCS: [(Source, u64); 4] = [
    (Source::Base, 0x755F_D7DF_75F7),
    (Source::Mobile, 0x7F7D_5DD5_7DFD),
    (Source::Direct(1), 0x5D57_7F77_57FF),
    (Source::Direct(2), 0x7DFF_D5F5_5D5F),
];

pub fn sync_pattern(s: Sync) -> u64 {
    let v = VOICE_SYNCS
        .iter()
        .find(|(src, _)| *src == s.source)
        .map(|(_, p)| *p)
        .unwrap();
    if s.voice { v } else { v ^ 0xAAAA_AAAA_AAAA }
}

/// Symbol (+3, +1, -1, -3 as 3, 1, -1, -3) of a dibit.
pub fn dibit_symbol(d: u8) -> i8 {
    match d & 3 {
        0b01 => 3,
        0b00 => 1,
        0b10 => -1,
        _ => -3,
    }
}

/// The sync pattern as 24 symbol signs (+1 for +3, -1 for -3).
pub fn sync_signs(pattern: u64) -> [i8; 24] {
    let mut s = [0i8; 24];
    for (i, v) in s.iter_mut().enumerate() {
        let d = ((pattern >> (46 - 2 * i)) & 3) as u8;
        *v = dibit_symbol(d).signum();
    }
    s
}

/// Data burst types (the slot type's data type field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    PiHeader,
    VoiceLcHeader,
    TerminatorWithLc,
    Csbk,
    MbcHeader,
    MbcContinuation,
    DataHeader,
    Rate12Data,
    Rate34Data,
    Idle,
    Rate1Data,
    Other(u8),
}

impl DataType {
    pub fn from_u8(v: u8) -> Self {
        use DataType::*;
        match v {
            0 => PiHeader,
            1 => VoiceLcHeader,
            2 => TerminatorWithLc,
            3 => Csbk,
            4 => MbcHeader,
            5 => MbcContinuation,
            6 => DataHeader,
            7 => Rate12Data,
            8 => Rate34Data,
            9 => Idle,
            10 => Rate1Data,
            v => Other(v),
        }
    }

    pub fn to_u8(self) -> u8 {
        use DataType::*;
        match self {
            PiHeader => 0,
            VoiceLcHeader => 1,
            TerminatorWithLc => 2,
            Csbk => 3,
            MbcHeader => 4,
            MbcContinuation => 5,
            DataHeader => 6,
            Rate12Data => 7,
            Rate34Data => 8,
            Idle => 9,
            Rate1Data => 10,
            Other(v) => v,
        }
    }
}

/// Slot type: colour code and data type, Golay (20,8) protected, in bits
/// 98..108 and 156..166. Returns `None` past three bit errors.
pub fn slot_type(b: &[u8]) -> Option<(u8, DataType)> {
    let w = (fec::to_u32(&b[98..108]) << 10) | fec::to_u32(&b[156..166]);
    let (d, dist) = fec::golay20_decode(w);
    (dist <= 3).then_some(((d >> 4) as u8, DataType::from_u8((d & 0xF) as u8)))
}

pub fn set_slot_type(b: &mut [u8], cc: u8, dt: DataType) {
    let w = fec::golay20_encode(((cc as u32 & 0xF) << 4) | dt.to_u8() as u32);
    fec::from_u32(w >> 10, 10, &mut b[98..108]);
    fec::from_u32(w & 0x3FF, 10, &mut b[156..166]);
}

/// The 196 BPTC-coded info bits of a data burst.
pub fn info_bits(b: &[u8]) -> [u8; 196] {
    let mut out = [0u8; 196];
    out[..98].copy_from_slice(&b[..98]);
    out[98..].copy_from_slice(&b[166..264]);
    out
}

pub fn set_info_bits(b: &mut [u8], info: &[u8; 196]) {
    b[..98].copy_from_slice(&info[..98]);
    b[166..264].copy_from_slice(&info[98..]);
}

pub fn set_sync(b: &mut [u8], pattern: u64) {
    fec::from_u32((pattern >> 24) as u32, 24, &mut b[108..132]);
    fec::from_u32((pattern & 0xFF_FFFF) as u32, 24, &mut b[132..156]);
}

/// EMB of voice bursts B-F: colour code, privacy indicator and link control
/// start/stop (LCSS). QR (16,7) protected; `None` past two bit errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emb {
    pub cc: u8,
    pub pi: bool,
    pub lcss: u8,
}

pub fn emb(b: &[u8]) -> Option<Emb> {
    let w = (fec::to_u32(&b[108..116]) << 8) | fec::to_u32(&b[148..156]);
    let (d, dist) = fec::qr16_decode(w);
    (dist <= 2).then_some(Emb {
        cc: (d >> 3) as u8 & 0xF,
        pi: d & 0b100 != 0,
        lcss: (d & 0b11) as u8,
    })
}

pub fn set_emb(b: &mut [u8], e: Emb, embedded: &[u8; 32]) {
    let d = ((e.cc as u32 & 0xF) << 3) | ((e.pi as u32) << 2) | (e.lcss as u32 & 3);
    let w = fec::qr16_encode(d);
    fec::from_u32(w >> 8, 8, &mut b[108..116]);
    b[116..148].copy_from_slice(embedded);
    fec::from_u32(w & 0xFF, 8, &mut b[148..156]);
}

pub fn embedded_bits(b: &[u8]) -> [u8; 32] {
    b[116..148].try_into().unwrap()
}

/// Timeslot bit of a CACH (the TC bit of its TACT).
pub fn cach_timeslot(cach: &[u8]) -> u8 {
    cach[4]
}

/// Bit positions of the TACT in the CACH.
const TACT: [usize; 7] = [0, 4, 8, 12, 14, 18, 22];

/// A CACH announcing timeslot `ts` (0 or 1) with the channel busy.
pub fn cach(ts: u8) -> [u8; 24] {
    let (at, tc, ls1, ls0) = (1u8, ts & 1, 0u8, 0u8);
    let tact = [
        at,
        tc,
        ls1,
        ls0,
        at ^ tc ^ ls1,
        tc ^ ls1 ^ ls0,
        at ^ tc ^ ls0,
    ];
    let mut c = [0u8; 24];
    for (p, v) in TACT.iter().zip(tact) {
        c[*p] = v;
    }
    c
}

/// Positions within a 72-bit AMBE+2 frame of its Golay (24,12) word C0,
/// whitened Golay (23,12) word C1, and unprotected bits C2+C3.
const AMBE_A: [usize; 24] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60, 64, 68, 1, 5, 9, 13, 17, 21,
];
const AMBE_B: [usize; 23] = [
    25, 29, 33, 37, 41, 45, 49, 53, 57, 61, 65, 69, 2, 6, 10, 14, 18, 22, 26, 30, 34, 38, 42,
];
const AMBE_C: [usize; 25] = [
    46, 50, 54, 58, 62, 66, 70, 3, 7, 11, 15, 19, 23, 27, 31, 35, 39, 43, 47, 51, 55, 59, 63, 67,
    71,
];

/// Burst bit of bit `j` of AMBE frame `k` (0..3): the voice payload skips
/// the 48 bits in the middle.
fn ambe_pos(k: usize, j: usize) -> usize {
    let v = 72 * k + j;
    if v < 108 { v } else { v + 48 }
}

/// One 20 ms AMBE+2 voice frame after error correction: the 49 parameter
/// bits, and how many bit errors the Golay codes corrected (a frame past
/// correction is marked `errs` >= 10, as in dsd's .amb files).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmbeFrame {
    pub bits: [u8; 49],
    pub errs: u8,
}

impl AmbeFrame {
    /// The frame as dsd writes it to an `.amb` file: error count, then the
    /// 49 bits packed into 7 bytes (the last holds bit 48 alone).
    pub fn amb_bytes(&self) -> [u8; 8] {
        let mut o = [0u8; 8];
        o[0] = self.errs;
        for i in 0..6 {
            o[1 + i] = fec::to_u32(&self.bits[i * 8..i * 8 + 8]) as u8;
        }
        o[7] = self.bits[48];
        o
    }

    pub fn from_amb_bytes(b: &[u8; 8]) -> Self {
        let mut bits = [0u8; 49];
        for i in 0..6 {
            fec::from_u32(b[1 + i] as u32, 8, &mut bits[i * 8..]);
        }
        bits[48] = b[7] & 1;
        AmbeFrame { bits, errs: b[0] }
    }
}

/// The whitening sequence for C1, seeded from C0's data bits.
fn ambe_prng(seed: u32) -> u32 {
    let mut pr = (16 * seed) & 0xFFFF;
    let mut mask = 0;
    for _ in 0..23 {
        pr = (173 * pr + 13849) & 0xFFFF;
        mask = (mask << 1) | (pr >> 15);
    }
    mask
}

/// The three voice frames of a voice burst.
pub fn ambe_frames(b: &[u8]) -> [AmbeFrame; 3] {
    std::array::from_fn(|k| {
        let read = |table: &[usize]| {
            table
                .iter()
                .fold(0u32, |a, &j| (a << 1) | b[ambe_pos(k, j)] as u32)
        };
        let (c0, c1, c23) = (read(&AMBE_A), read(&AMBE_B), read(&AMBE_C));
        let (d0, e0, bad) = fec::golay24_decode(c0);
        let (d1, e1) = fec::golay23_decode(c1 ^ ambe_prng(d0));
        let mut bits = [0u8; 49];
        fec::from_u32(d0, 12, &mut bits[0..]);
        fec::from_u32(d1, 12, &mut bits[12..]);
        fec::from_u32(c23, 25, &mut bits[24..]);
        let errs = if bad { 10 } else { (e0 + e1) as u8 };
        AmbeFrame { bits, errs }
    })
}

/// Writes three voice frames into a burst (for the synthesiser).
pub fn set_ambe_frames(b: &mut [u8], frames: &[[u8; 49]; 3]) {
    for (k, f) in frames.iter().enumerate() {
        let d0 = fec::to_u32(&f[0..12]);
        let c0 = fec::golay24_encode(d0);
        let c1 = fec::golay23_encode(fec::to_u32(&f[12..24])) ^ ambe_prng(d0);
        let c23 = fec::to_u32(&f[24..49]);
        for (table, v, n) in [
            (&AMBE_A[..], c0, 24),
            (&AMBE_B[..], c1, 23),
            (&AMBE_C[..], c23, 25),
        ] {
            for (i, &j) in table.iter().enumerate() {
                b[ambe_pos(k, j)] = ((v >> (n - 1 - i)) & 1) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_syncs_negate_voice_syncs() {
        for (src, _) in VOICE_SYNCS {
            let v = sync_signs(sync_pattern(Sync {
                source: src,
                voice: true,
            }));
            let d = sync_signs(sync_pattern(Sync {
                source: src,
                voice: false,
            }));
            assert!(v.iter().zip(d).all(|(a, b)| *a == -b));
        }
        // Sync uses only the outer symbols.
        assert_eq!(&sync_signs(0x755F_D7DF_75F7)[..4], &[1, -1, 1, 1]);
    }

    #[test]
    fn ambe_round_trip() {
        let mut b = [0u8; 264];
        let frames: [[u8; 49]; 3] =
            std::array::from_fn(|k| std::array::from_fn(|i| ((i * 3 + k) % 4 == 1) as u8));
        set_ambe_frames(&mut b, &frames);
        let got = ambe_frames(&b);
        for k in 0..3 {
            assert_eq!(got[k].bits, frames[k]);
            assert_eq!(got[k].errs, 0);
        }
        // The middle 48 bits are not voice.
        assert!(b[108..156].iter().all(|&v| v == 0));
        b[0] ^= 1;
        b[200] ^= 1;
        let got = ambe_frames(&b);
        assert_eq!(got[0].bits, frames[0]);
        assert_eq!(got[0].errs, 1);
        let f = AmbeFrame::from_amb_bytes(&got[2].amb_bytes());
        assert_eq!(f, got[2]);
    }

    #[test]
    fn slot_type_and_emb_round_trip() {
        let mut b = [0u8; 264];
        set_slot_type(&mut b, 7, DataType::TerminatorWithLc);
        b[100] ^= 1;
        assert_eq!(slot_type(&b), Some((7, DataType::TerminatorWithLc)));
        let e = Emb {
            cc: 1,
            pi: false,
            lcss: 3,
        };
        let emb_bits = [1u8; 32];
        set_emb(&mut b, e, &emb_bits);
        assert_eq!(emb(&b), Some(e));
        assert_eq!(embedded_bits(&b), emb_bits);
    }
}
