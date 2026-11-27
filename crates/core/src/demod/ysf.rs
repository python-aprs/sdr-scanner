//! System Fusion channels: discriminator output into the C4FM receiver,
//! one call at a time out (see [`crate::ysf`]).

use super::{Call, CallInfo, Demod};
use crate::config::Mode;
use crate::dmr::vocoder::{self, FRAME_SAMPLES, Leveled, Vocoder};
use crate::ysf::calls::{CallEvent, Calls};
use crate::ysf::rx::{self, Receiver, RxFrame};
use num_complex::Complex32;

/// Makes calls, never channel audio: like DMR, a recording is a call
/// (header to terminator), named by who made it.
pub struct YsfVoice {
    rx: Receiver,
    calls: Calls,
    rate: f64,
    frames: Vec<RxFrame>,
    events: Vec<CallEvent>,
    out: Vec<Call>,
    vocoder: Option<Box<dyn Vocoder>>,
    /// Frames received (the receiver only passes frames while locked).
    received: u32,
}

impl YsfVoice {
    pub fn new(rate: f64) -> Self {
        YsfVoice {
            rx: Receiver::new(rate),
            calls: Calls::new(),
            rate,
            frames: Vec::new(),
            events: Vec::new(),
            out: Vec::new(),
            vocoder: None,
            received: 0,
        }
    }

    /// Channel IQ sample of a 48 kHz receiver position.
    fn iq_sample(&self, at: u64) -> u64 {
        ((at as f64 - self.rx.delay()).max(0.0) * self.rate / rx::RATE) as u64
    }

    fn convert(&mut self) {
        for e in std::mem::take(&mut self.events) {
            match e {
                CallEvent::Start { at, info } => {
                    self.vocoder =
                        vocoder::new().map(|v| Box::new(Leveled::new(v)) as Box<dyn Vocoder>);
                    self.out.push(Call::Start {
                        key: 0,
                        at_sample: self.iq_sample(at),
                        info: CallInfo::Ysf(info),
                    });
                }
                CallEvent::Voice { frames } => {
                    let mut audio = Vec::with_capacity(frames.len() * FRAME_SAMPLES);
                    match &mut self.vocoder {
                        Some(v) => frames.iter().for_each(|f| v.decode(f, &mut audio)),
                        None => audio.resize(frames.len() * FRAME_SAMPLES, 0.0),
                    }
                    self.out.push(Call::Voice {
                        key: 0,
                        frames,
                        audio,
                    });
                }
                CallEvent::End { at, info } => {
                    self.vocoder = None;
                    self.out.push(Call::End {
                        key: 0,
                        at_sample: self.iq_sample(at),
                        info: CallInfo::Ysf(info),
                    });
                }
            }
        }
    }
}

impl Demod for YsfVoice {
    fn process(&mut self, iq: &[Complex32], _open: bool, _audio: &mut Vec<f32>) {
        self.rx.push(iq, &mut self.frames);
        self.received += self.frames.len() as u32;
        for f in self.frames.drain(..) {
            self.calls.push(&f, &mut self.events);
        }
        self.convert();
    }

    fn flush(&mut self, _audio: &mut Vec<f32>) {
        self.calls.end_all(&mut self.events);
        self.convert();
    }

    fn records_audio(&self) -> bool {
        false
    }

    fn detected(&self) -> Option<Mode> {
        (self.received >= 2).then_some(Mode::Ysf)
    }

    fn take_calls(&mut self) -> Vec<Call> {
        std::mem::take(&mut self.out)
    }
}
