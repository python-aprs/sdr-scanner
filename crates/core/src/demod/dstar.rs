//! D-STAR channels: discriminator output into the GMSK receiver, one call
//! at a time out (see [`crate::dstar`]).

use super::{Call, CallInfo, Demod};
use crate::dmr::vocoder::{self, Codec, FRAME_SAMPLES, Leveled, Vocoder};
use crate::dstar::calls::{CallEvent, Calls};
use crate::dstar::rx::{self, Receiver, RxEvent};
use num_complex::Complex32;

/// Makes calls, never channel audio: a recording is a call, header to end
/// pattern, named by who made it.
pub struct DstarVoice {
    rx: Receiver,
    calls: Calls,
    rate: f64,
    rx_events: Vec<RxEvent>,
    events: Vec<CallEvent>,
    out: Vec<Call>,
    vocoder: Option<Box<dyn Vocoder>>,
}

impl DstarVoice {
    pub fn new(rate: f64) -> Self {
        DstarVoice {
            rx: Receiver::new(rate),
            calls: Calls::new(),
            rate,
            rx_events: Vec::new(),
            events: Vec::new(),
            out: Vec::new(),
            vocoder: None,
        }
    }

    fn iq_sample(&self, at: u64) -> u64 {
        ((at as f64 - self.rx.delay()).max(0.0) * self.rate / rx::RATE) as u64
    }

    fn convert(&mut self) {
        for e in std::mem::take(&mut self.events) {
            match e {
                CallEvent::Start { at, info } => {
                    self.vocoder = vocoder::new_for(Codec::Ambe)
                        .map(|v| Box::new(Leveled::new(v)) as Box<dyn Vocoder>);
                    self.out.push(Call::Start {
                        key: 0,
                        at_sample: self.iq_sample(at),
                        info: CallInfo::Dstar(info),
                    });
                }
                CallEvent::Voice { frame } => {
                    let mut audio = Vec::with_capacity(FRAME_SAMPLES);
                    match &mut self.vocoder {
                        Some(v) => v.decode(&frame, &mut audio),
                        None => audio.resize(FRAME_SAMPLES, 0.0),
                    }
                    self.out.push(Call::Voice {
                        key: 0,
                        frames: vec![frame],
                        audio,
                    });
                }
                CallEvent::End { at, info } => {
                    self.vocoder = None;
                    self.out.push(Call::End {
                        key: 0,
                        at_sample: self.iq_sample(at),
                        info: CallInfo::Dstar(info),
                    });
                }
            }
        }
    }
}

impl Demod for DstarVoice {
    fn process(&mut self, iq: &[Complex32], _open: bool, _audio: &mut Vec<f32>) {
        self.rx.push(iq, &mut self.rx_events);
        for e in self.rx_events.drain(..) {
            self.calls.push(&e, &mut self.events);
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

    fn take_calls(&mut self) -> Vec<Call> {
        std::mem::take(&mut self.out)
    }
}
