//! AMBE+2 voice frames to audio.
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

/// A vocoder for one stream, if this build has one.
pub fn new() -> Option<Box<dyn Vocoder>> {
    #[cfg(feature = "mbelib")]
    {
        mbe::Mbe::new().map(|m| Box::new(m) as Box<dyn Vocoder>)
    }
    #[cfg(not(feature = "mbelib"))]
    {
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
        fn scanner_mbe_decode(p: *mut c_void, ambe_d: *const c_char, errs: c_int, out: *mut f32);
    }

    pub struct Mbe(*mut c_void);

    // The state is only touched through &mut self.
    unsafe impl Send for Mbe {}

    impl Mbe {
        pub fn new() -> Option<Self> {
            let p = unsafe { scanner_mbe_new() };
            (!p.is_null()).then_some(Mbe(p))
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
                scanner_mbe_decode(self.0, bits.as_ptr(), f.errs as c_int, buf.as_mut_ptr());
            }
            // mbelib's float output is scaled like 16-bit samples.
            out.extend(buf.iter().map(|v| v / 32768.0));
        }
    }
}
