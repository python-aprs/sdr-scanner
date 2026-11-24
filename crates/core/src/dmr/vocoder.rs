//! AMBE+2 (DMR, System Fusion) and AMBE (D-STAR) voice frames to audio.
//!
//! The codec is DVSI's and patented, so the default build has no vocoder:
//! calls are recorded as silence with their voice frames kept beside them.
//! Building with the `mbelib` feature links mbelib-neo (or classic mbelib), open-source decoders,
//! which turns the frames into 8 kHz audio.

use super::burst::AmbeFrame;
use crate::dsp::Agc;

/// Audio rate of decoded voice.
pub const AUDIO_RATE: u32 = 8000;
/// Samples per voice frame (20 ms).
pub const FRAME_SAMPLES: usize = 160;

pub trait Vocoder: Send {
    /// Decode one frame, appending 160 samples.
    fn decode(&mut self, f: &AmbeFrame, out: &mut Vec<f32>);
}

/// Which codec voice frames are for. Both have 49 parameter bits a frame,
/// but they mean different things.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Codec {
    /// AMBE+2 3600x2450: DMR and System Fusion.
    #[default]
    AmbePlus2,
    /// AMBE 3600x2400: D-STAR.
    Ambe,
}

/// An AMBE+2 vocoder for one stream, if this build has one.
pub fn new() -> Option<Box<dyn Vocoder>> {
    new_for(Codec::AmbePlus2)
}

/// A vocoder for `codec`, if this build has one.
pub fn new_for(codec: Codec) -> Option<Box<dyn Vocoder>> {
    #[cfg(feature = "mbelib")]
    {
        mbe::Mbe::new(codec).map(|m| Box::new(m) as Box<dyn Vocoder>)
    }
    #[cfg(not(feature = "mbelib"))]
    {
        let _ = codec;
        None
    }
}

/// Whether voice can be decoded (the `mbelib` feature, built against the
/// library).
pub fn available() -> bool {
    new().is_some()
}

/// Levels decoded voice like the FM path's recordings.
pub struct Leveled {
    inner: Box<dyn Vocoder>,
    agc: Agc,
}

impl Leveled {
    pub fn new(inner: Box<dyn Vocoder>) -> Self {
        Leveled {
            inner,
            agc: Agc::new(AUDIO_RATE as f64),
        }
    }
}

impl Vocoder for Leveled {
    fn decode(&mut self, f: &AmbeFrame, out: &mut Vec<f32>) {
        let n = out.len();
        self.inner.decode(f, out);
        self.agc.process(&mut out[n..], true);
    }
}

#[cfg(feature = "mbelib")]
mod mbe {
    use super::*;
    use std::ffi::{c_char, c_int, c_void};

    unsafe extern "C" {
        fn scanner_mbe_new() -> *mut c_void;
        fn scanner_mbe_free(p: *mut c_void);
        fn scanner_mbe_decode(
            p: *mut c_void,
            ambe_d: *const c_char,
            errs: c_int,
            dstar: c_int,
            out: *mut f32,
        );
    }

    pub struct Mbe(*mut c_void, Codec);

    // The state is only touched through &mut self.
    unsafe impl Send for Mbe {}

    impl Mbe {
        pub fn new(codec: Codec) -> Option<Self> {
            let p = unsafe { scanner_mbe_new() };
            (!p.is_null()).then_some(Mbe(p, codec))
        }
    }

    impl Drop for Mbe {
        fn drop(&mut self) {
            unsafe { scanner_mbe_free(self.0) }
        }
    }

    impl Vocoder for Mbe {
        fn decode(&mut self, f: &AmbeFrame, out: &mut Vec<f32>) {
            let mut buf = [0f32; FRAME_SAMPLES];
            let bits: [c_char; 49] = f.bits.map(|b| b as c_char);
            unsafe {
                scanner_mbe_decode(
                    self.0,
                    bits.as_ptr(),
                    f.errs as c_int,
                    (self.1 == Codec::Ambe) as c_int,
                    buf.as_mut_ptr(),
                );
            }
            // mbelib's float output is scaled like 16-bit samples.
            out.extend(buf.iter().map(|v| v / 32768.0));
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "mbelib")]
    use super::*;
    use crate::dmr::{burst, fec};

    /// A voice burst of three AMBE+2 silence frames, as MMDVMHost sends it.
    const SILENCE_BURST: [u8; 33] = [
        0xB9, 0xE8, 0x81, 0x52, 0x61, 0x73, 0x00, 0x2A, 0x6B, 0xB9, 0xE8, 0x81, 0x52, 0x60, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x73, 0x00, 0x2A, 0x6B, 0xB9, 0xE8, 0x81, 0x52, 0x61, 0x73,
        0x00, 0x2A, 0x6B,
    ];

    #[test]
    fn silence_frames_decode_without_errors() {
        // Real AMBE+2 codewords: the frame layout, Golay codes and C1
        // whitening all have to be right for these to come out clean.
        let bits = fec::bytes_to_bits(&SILENCE_BURST);
        let frames = burst::ambe_frames(&bits);
        for f in &frames {
            assert_eq!(f.errs, 0);
            assert_eq!(f.bits, frames[0].bits);
        }
    }

    #[cfg(feature = "mbelib")]
    #[test]
    fn mbelib_decodes_frames() {
        let Some(mut v) = new() else {
            eprintln!("built without the mbelib library; skipping");
            return;
        };
        let bits = fec::bytes_to_bits(&SILENCE_BURST);
        let mut out = Vec::new();
        for _ in 0..10 {
            for f in burst::ambe_frames(&bits) {
                v.decode(&f, &mut out);
            }
        }
        assert_eq!(out.len(), 30 * FRAME_SAMPLES);
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        let quiet = rms(&out);
        // Voice parameters make sound.
        let mut out = Vec::new();
        for n in 0..30 {
            let f = burst::AmbeFrame {
                bits: crate::dmr::tx::test_frame(n),
                errs: 0,
            };
            v.decode(&f, &mut out);
        }
        let loud = rms(&out);
        assert!(loud > 10.0 * quiet, "voice {loud} vs silence {quiet}");
    }

    #[cfg(feature = "mbelib")]
    #[test]
    fn mbelib_decodes_dstar_frames() {
        let Some(mut v) = new_for(Codec::Ambe) else {
            eprintln!("built without the mbelib library; skipping");
            return;
        };
        // D-STAR's silence frame decodes quiet; test parameters make sound.
        let null = [0x9E, 0x8D, 0x32, 0x88, 0x26, 0x1A, 0x3F, 0x61, 0xE8u8];
        let bits: Vec<u8> = null
            .iter()
            .flat_map(|b| (0..8).map(move |i| (b >> i) & 1))
            .collect();
        let f = crate::dstar::frame::voice(&bits);
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        let mut out = Vec::new();
        for _ in 0..30 {
            v.decode(&f, &mut out);
        }
        let quiet = rms(&out);
        let mut out = Vec::new();
        for n in 0..30 {
            let f = burst::AmbeFrame {
                bits: crate::dmr::tx::test_frame(n),
                errs: 0,
            };
            v.decode(&f, &mut out);
        }
        let loud = rms(&out);
        assert!(loud > 10.0 * quiet, "voice {loud} vs silence {quiet}");
    }
}
